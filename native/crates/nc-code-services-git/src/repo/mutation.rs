//! Repository writes: branch switch/create, stage/unstage/discard, commit, push.
//!
//! Port of the mutation half of `packages/services/src/git/repo/gitCliRepo.ts`. Every
//! user-controlled value reaches Git as one argv entry, and pathspecs always follow an
//! explicit `--` so a path that starts with `-` cannot be re-parsed as an option
//! (`gitCliRepo.ts:1220-1231`).

use std::path::PathBuf;

use nc_code_service_core::ServiceError;
use serde_json::{json, Value};

use crate::cmd::{GitCommand, PUSH_OUTPUT_BYTES, PUSH_TIMEOUT};
use crate::parse::{
    ensure_git_command_succeeded, parse_git_branch_mutation_issues, to_invalid_branch_name_issue,
};
use crate::refs::{parse_index_entries, IndexEntry};
use crate::repo::{GitRepo, ResolvedRepository, StatusSnapshot};

/// `finally { rm(tempIndexDir) }` — the temporary index must never survive a failure.
struct TempIndexDir(PathBuf);

impl TempIndexDir {
    async fn create() -> Result<Self, ServiceError> {
        let root = std::env::temp_dir();
        for attempt in 0..64u32 {
            let candidate = root.join(format!(
                "zcode-git-index-{}-{attempt}-{}",
                std::process::id(),
                nanos_suffix()
            ));
            match tokio::fs::create_dir(&candidate).await {
                Ok(()) => return Ok(Self(candidate)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(ServiceError::message(error.to_string())),
            }
        }
        Err(ServiceError::message(
            "Failed to create a temporary Git index directory",
        ))
    }

    fn index_path(&self) -> PathBuf {
        self.0.join("index")
    }
}

impl Drop for TempIndexDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn nanos_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

impl GitRepo {
    /// `validateBranchName` (`gitCliRepo.ts:600`).
    async fn validate_branch_name(
        &self,
        resolution: &ResolvedRepository,
        branch_name: &str,
    ) -> Result<Option<Value>, ServiceError> {
        let result = self
            .provider
            .run(GitCommand::new(
                resolution.root(),
                &["check-ref-format", "--branch", branch_name],
            ))
            .await?;
        if result.timed_out || result.output_truncated {
            ensure_git_command_succeeded("git check-ref-format --branch", &result, &[0])?;
        }
        if result.exit_code == Some(0) {
            return Ok(None);
        }
        Ok(Some(to_invalid_branch_name_issue(Some(&result.stderr))))
    }

    /// `switchBranch` (`gitCliRepo.ts:1071`).
    pub async fn switch_branch(
        &self,
        workspace_path: &str,
        target_branch_name: &str,
    ) -> Result<Value, ServiceError> {
        let status = self.get_status(workspace_path).await?;
        Self::ensure_repository_available(&status.resolution, "switch branches")?;
        let normalized = target_branch_name.trim().to_string();
        if normalized.is_empty() {
            return Ok(branch_mutation_failure(
                "switch",
                Value::Null,
                false,
                status.summary.clone(),
                vec![to_invalid_branch_name_issue(None)],
            ));
        }
        if status.summary["headRefType"] == json!("branch")
            && status.summary["branchName"] == json!(normalized.clone())
        {
            return Ok(branch_mutation_success(
                "switch",
                &normalized,
                false,
                false,
                status.summary,
            ));
        }
        // Report the blockers the repository already knows about before asking Git, so the
        // UI never has to parse a version-specific native message (`gitCliRepo.ts:1100`).
        if status.entries.iter().any(|entry| entry.is_conflicted) {
            return Ok(branch_mutation_failure(
                "switch",
                json!(normalized),
                false,
                status.summary,
                vec![json!({
                    "code": "conflicts-present",
                    "message": "Repository still has unresolved conflicts.",
                })],
            ));
        }
        if self.has_operation_in_progress(&status.resolution).await? {
            return Ok(branch_mutation_failure(
                "switch",
                json!(normalized),
                false,
                status.summary,
                vec![json!({
                    "code": "operation-in-progress",
                    "message": "Another Git operation is still in progress.",
                })],
            ));
        }
        if let Some(issue) = self
            .validate_branch_name(&status.resolution, &normalized)
            .await?
        {
            return Ok(branch_mutation_failure(
                "switch",
                json!(normalized),
                false,
                status.summary,
                vec![issue],
            ));
        }
        let result = self
            .provider
            .run(GitCommand::new(
                status.resolution.root(),
                &["switch", "--no-guess", &normalized],
            ))
            .await?;
        if result.exit_code != Some(0) {
            return Ok(branch_mutation_failure(
                "switch",
                json!(normalized),
                false,
                status.summary,
                parse_git_branch_mutation_issues(&result),
            ));
        }
        let next = self.get_status(workspace_path).await?;
        Ok(branch_mutation_success(
            "switch",
            &normalized,
            true,
            false,
            next.summary,
        ))
    }

    /// `createBranchAndSwitch` (`gitCliRepo.ts:1165`).
    pub async fn create_branch_and_switch(
        &self,
        workspace_path: &str,
        branch_name: &str,
        start_point: Option<&str>,
    ) -> Result<Value, ServiceError> {
        let status = self.get_status(workspace_path).await?;
        Self::ensure_repository_available(&status.resolution, "create and switch branches")?;
        let normalized = branch_name.trim().to_string();
        if normalized.is_empty() {
            return Ok(branch_mutation_failure(
                "create-and-switch",
                Value::Null,
                true,
                status.summary.clone(),
                vec![to_invalid_branch_name_issue(None)],
            ));
        }
        if status.entries.iter().any(|entry| entry.is_conflicted) {
            return Ok(branch_mutation_failure(
                "create-and-switch",
                json!(normalized),
                true,
                status.summary,
                vec![json!({
                    "code": "conflicts-present",
                    "message": "Repository still has unresolved conflicts.",
                })],
            ));
        }
        if self.has_operation_in_progress(&status.resolution).await? {
            return Ok(branch_mutation_failure(
                "create-and-switch",
                json!(normalized),
                true,
                status.summary,
                vec![json!({
                    "code": "operation-in-progress",
                    "message": "Another Git operation is still in progress.",
                })],
            ));
        }
        if let Some(issue) = self
            .validate_branch_name(&status.resolution, &normalized)
            .await?
        {
            return Ok(branch_mutation_failure(
                "create-and-switch",
                json!(normalized),
                true,
                status.summary,
                vec![issue],
            ));
        }
        let normalized_start_point = start_point.map(str::trim).filter(|value| !value.is_empty());
        // A start point that begins with `-` would otherwise be parsed as a switch option,
        // so `--` terminates option parsing (`gitCliRepo.ts:1220-1228`).
        let result = match normalized_start_point {
            Some(start_point) => {
                self.provider
                    .run(GitCommand::new(
                        status.resolution.root(),
                        &["switch", "--no-guess", "-c", &normalized, "--", start_point],
                    ))
                    .await?
            }
            None => {
                self.provider
                    .run(GitCommand::new(
                        status.resolution.root(),
                        &["switch", "--no-guess", "-c", &normalized],
                    ))
                    .await?
            }
        };
        if result.exit_code != Some(0) {
            return Ok(branch_mutation_failure(
                "create-and-switch",
                json!(normalized),
                true,
                status.summary,
                parse_git_branch_mutation_issues(&result),
            ));
        }
        let next = self.get_status(workspace_path).await?;
        Ok(branch_mutation_success(
            "create-and-switch",
            &normalized,
            true,
            true,
            next.summary,
        ))
    }

    /// `stage` / `unstage` / `discard` share the path-normalisation and empty-input rules.
    /// `success_label` is the exact TS error label (`git add`, `git restore --staged`,
    /// `git restore`).
    async fn mutate_paths(
        &self,
        workspace_path: &str,
        paths: &[String],
        label: &str,
        success_label: &str,
        argv_prefix: &[&str],
    ) -> Result<(), ServiceError> {
        let resolution = self.resolve_repository(workspace_path).await;
        Self::ensure_repository_available(&resolution, label)?;
        let mut repo_paths: Vec<String> = Vec::new();
        for path in paths {
            let normalized = self.normalize_input_path(&resolution, path).await?;
            if !repo_paths.contains(&normalized) {
                repo_paths.push(normalized);
            }
        }
        if repo_paths.is_empty() {
            return Ok(());
        }
        let mut args: Vec<&str> = argv_prefix.to_vec();
        args.push("--");
        args.extend(repo_paths.iter().map(String::as_str));
        let result = self
            .provider
            .run(GitCommand::new(resolution.root(), &args))
            .await?;
        ensure_git_command_succeeded(success_label, &result, &[0])
    }

    pub async fn stage(&self, workspace_path: &str, paths: &[String]) -> Result<(), ServiceError> {
        self.mutate_paths(workspace_path, paths, "stage paths", "git add", &["add"])
            .await
    }

    pub async fn unstage(
        &self,
        workspace_path: &str,
        paths: &[String],
    ) -> Result<(), ServiceError> {
        self.mutate_paths(
            workspace_path,
            paths,
            "unstage paths",
            "git restore --staged",
            &["restore", "--staged"],
        )
        .await
    }

    /// `discard` (`gitCliRepo.ts:1465`).
    pub async fn discard(
        &self,
        workspace_path: &str,
        paths: &[String],
        staged: bool,
    ) -> Result<(), ServiceError> {
        let prefix = if staged {
            &["restore", "--source=HEAD", "--staged", "--worktree"][..]
        } else {
            &["restore", "--worktree"][..]
        };
        self.mutate_paths(
            workspace_path,
            paths,
            "discard paths",
            "git restore",
            prefix,
        )
        .await
    }

    /// `commit` (`gitCliRepo.ts:1488`).
    pub async fn commit(
        &self,
        workspace_path: &str,
        message: &str,
        paths: Option<&[String]>,
        staged_only: bool,
    ) -> Result<Value, ServiceError> {
        let resolution = self.resolve_repository(workspace_path).await;
        Self::ensure_repository_available(&resolution, "commit changes")?;
        let trimmed_message = message.trim().to_string();
        if trimmed_message.is_empty() {
            return Err(ServiceError::message("Commit message cannot be empty"));
        }
        let mut repo_paths: Vec<String> = Vec::new();
        for path in paths.unwrap_or_default() {
            let normalized = self.normalize_input_path(&resolution, path).await?;
            if !repo_paths.contains(&normalized) {
                repo_paths.push(normalized);
            }
        }

        if staged_only && !repo_paths.is_empty() {
            return self
                .commit_selected_staged(&resolution, &trimmed_message, &repo_paths)
                .await;
        }

        let mut args: Vec<&str> = vec!["commit", "-m", trimmed_message.as_str()];
        if !repo_paths.is_empty() {
            args.push("--");
            args.extend(repo_paths.iter().map(String::as_str));
        }
        let result = self
            .provider
            .run(GitCommand::new(resolution.root(), &args))
            .await?;
        ensure_git_command_succeeded("git commit", &result, &[0])?;
        let hash = self
            .provider
            .run(GitCommand::new(resolution.root(), &["rev-parse", "HEAD"]))
            .await?;
        ensure_git_command_succeeded("git rev-parse HEAD", &hash, &[0])?;
        Ok(json!({"commitHash": hash.stdout.trim()}))
    }

    /// The `stagedOnly` branch (`gitCliRepo.ts:1509-1615`): committing only the selected
    /// staged paths must not swallow the rest of the real index, so the commit runs
    /// against a temporary `GIT_INDEX_FILE` seeded from HEAD plus the selected entries,
    /// and only those paths are then re-synced onto the new HEAD.
    async fn commit_selected_staged(
        &self,
        resolution: &ResolvedRepository,
        message: &str,
        repo_paths: &[String],
    ) -> Result<Value, ServiceError> {
        let repo_root = resolution.root().to_path_buf();
        let mut path_arguments: Vec<&str> = Vec::new();
        path_arguments.extend(repo_paths.iter().map(String::as_str));

        let scoped_status = self
            .provider
            .run(GitCommand::new(
                &repo_root,
                &interleaved(&["status", "--porcelain=v2", "-z", "--"], &path_arguments),
            ))
            .await?;
        ensure_git_command_succeeded("git status selected paths", &scoped_status, &[0])?;
        let parsed = crate::parse::parse_status_porcelain(&scoped_status.stdout);
        // A staged rename must be removed from the temporary index under both names.
        let mut cleanup_paths: Vec<String> = repo_paths.to_vec();
        for entry in &parsed.entries {
            if repo_paths.contains(&entry.path) {
                if let Some(original) = &entry.original_path {
                    if !cleanup_paths.contains(original) {
                        cleanup_paths.push(original.clone());
                    }
                }
            }
        }

        let staged_entries_result = self
            .provider
            .run(GitCommand::new(
                &repo_root,
                &interleaved(&["ls-files", "--stage", "-z", "--"], &path_arguments),
            ))
            .await?;
        ensure_git_command_succeeded(
            "git ls-files selected staged entries",
            &staged_entries_result,
            &[0],
        )?;
        let staged_entries: Vec<IndexEntry> = parse_index_entries(&staged_entries_result.stdout)?;
        if staged_entries.iter().any(|entry| entry.stage != "0") {
            return Err(ServiceError::message(
                "Cannot commit selected staged paths while index conflicts exist.",
            ));
        }

        let head = self
            .provider
            .run(GitCommand::new(
                &repo_root,
                &["rev-parse", "--verify", "HEAD"],
            ))
            .await?;
        let parent_hash = if head.exit_code == Some(0) {
            let trimmed = head.stdout.trim().to_string();
            (!trimmed.is_empty()).then_some(trimmed)
        } else {
            None
        };

        let temporary = TempIndexDir::create().await?;
        let index_file = temporary.index_path().to_string_lossy().into_owned();
        let extra_env = [("GIT_INDEX_FILE", index_file.as_str())];

        let read_tree_args: Vec<&str> = match &parent_hash {
            Some(parent) => vec!["read-tree", parent.as_str()],
            None => vec!["read-tree", "--empty"],
        };
        let read_tree = self
            .provider
            .run(GitCommand::new(&repo_root, &read_tree_args).extra_env(&extra_env))
            .await?;
        ensure_git_command_succeeded("git read-tree selected commit base", &read_tree, &[0])?;

        if !cleanup_paths.is_empty() {
            let mut cleanup_arguments: Vec<&str> =
                cleanup_paths.iter().map(String::as_str).collect();
            let mut args = vec!["update-index", "--force-remove", "--"];
            args.append(&mut cleanup_arguments);
            let remove = self
                .provider
                .run(GitCommand::new(&repo_root, &args).extra_env(&extra_env))
                .await?;
            ensure_git_command_succeeded("git update-index remove selected paths", &remove, &[0])?;
        }

        for entry in &staged_entries {
            let add = self
                .provider
                .run(
                    GitCommand::new(
                        &repo_root,
                        &[
                            "update-index",
                            "--add",
                            "--cacheinfo",
                            &entry.mode,
                            &entry.object_hash,
                            &entry.path,
                        ],
                    )
                    .extra_env(&extra_env),
                )
                .await?;
            ensure_git_command_succeeded("git update-index add selected paths", &add, &[0])?;
        }

        let scoped_commit = self
            .provider
            .run(GitCommand::new(&repo_root, &["commit", "-m", message]).extra_env(&extra_env))
            .await?;
        ensure_git_command_succeeded("git commit selected staged paths", &scoped_commit, &[0])?;

        let hash = self
            .provider
            .run(GitCommand::new(&repo_root, &["rev-parse", "HEAD"]))
            .await?;
        ensure_git_command_succeeded("git rev-parse selected commit HEAD", &hash, &[0])?;
        let commit_hash = hash.stdout.trim().to_string();

        // Sync only the committed paths onto the new HEAD; other staged files stay staged
        // for the user (`gitCliRepo.ts:1602-1609`).
        let mut reset_arguments: Vec<&str> = cleanup_paths.iter().map(String::as_str).collect();
        let mut args = vec!["reset", "--quiet", "HEAD", "--"];
        args.append(&mut reset_arguments);
        let reset = self
            .provider
            .run(GitCommand::new(&repo_root, &args))
            .await?;
        ensure_git_command_succeeded("git reset selected committed paths", &reset, &[0])?;
        Ok(json!({"commitHash": commit_hash}))
    }

    /// `push` (`gitCliRepo.ts:1638`).
    pub async fn push(&self, workspace_path: &str) -> Result<Value, ServiceError> {
        let status = self.get_status(workspace_path).await?;
        Self::ensure_repository_available(&status.resolution, "push changes")?;
        let branch_name = status.summary["branchName"]
            .as_str()
            .map(str::trim)
            .unwrap_or_default()
            .to_string();
        if status.summary["headRefType"] != json!("branch") || branch_name.is_empty() {
            return Err(ServiceError::message("Cannot push while HEAD is detached."));
        }
        let tracking_branch_name = status.summary["trackingBranchName"]
            .as_str()
            .map(str::to_string);
        let has_tracking_branch = tracking_branch_name.is_some();
        let remote_name = match &tracking_branch_name {
            Some(tracking) => parse_tracking_remote_name(Some(tracking)),
            None => self.resolve_push_remote(&status).await?,
        };
        let repo_root = status.resolution.root().to_path_buf();
        let result = if has_tracking_branch {
            self.provider
                .run(
                    GitCommand::new(&repo_root, &["push"])
                        .timeout(PUSH_TIMEOUT)
                        .max_output_bytes(PUSH_OUTPUT_BYTES),
                )
                .await?
        } else {
            let remote = remote_name.clone().unwrap_or_else(|| "origin".to_string());
            self.provider
                .run(
                    GitCommand::new(
                        &repo_root,
                        &["push", "--set-upstream", &remote, &branch_name],
                    )
                    .timeout(PUSH_TIMEOUT)
                    .max_output_bytes(PUSH_OUTPUT_BYTES),
                )
                .await?
        };
        ensure_git_command_succeeded("git push", &result, &[0])?;
        let next = self.get_status(workspace_path).await?;
        let next_tracking = next.summary["trackingBranchName"]
            .as_str()
            .map(str::to_string);
        Ok(json!({
            "branchName": branch_name,
            "trackingBranchName": next_tracking.clone(),
            "remoteName": remote_name.or_else(|| parse_tracking_remote_name(next_tracking.as_deref())),
            "setUpstream": !has_tracking_branch,
            "summary": next.summary,
        }))
    }

    /// `resolvePushRemote` (`gitCliRepo.ts:674`).
    async fn resolve_push_remote(
        &self,
        status: &StatusSnapshot,
    ) -> Result<Option<String>, ServiceError> {
        let resolution = &status.resolution;
        Self::ensure_repository_available(resolution, "push changes")?;
        let branch_name = status.summary["branchName"]
            .as_str()
            .map(str::trim)
            .unwrap_or_default();
        if branch_name.is_empty() {
            return Err(ServiceError::message(
                "Cannot resolve a Git push remote without a current branch.",
            ));
        }
        if let Some(branch_remote) = self
            .read_optional_git_config(resolution, &format!("branch.{branch_name}.remote"))
            .await?
        {
            return Ok(Some(branch_remote));
        }
        if let Some(push_default) = self
            .read_optional_git_config(resolution, "remote.pushDefault")
            .await?
        {
            return Ok(Some(push_default));
        }
        let remotes = self.list_remotes(resolution).await?;
        if remotes.iter().any(|remote| remote == "origin") {
            return Ok(Some("origin".to_string()));
        }
        if remotes.len() == 1 {
            return Ok(remotes.first().cloned());
        }
        if remotes.is_empty() {
            return Err(ServiceError::message(
                "No Git remote is configured for the current repository.",
            ));
        }
        Err(ServiceError::message(
            "Multiple Git remotes are configured. Configure branch.<name>.remote or remote.pushDefault first.",
        ))
    }
}

/// `parseTrackingRemoteName` (`gitCliRepo.ts:499`).
pub fn parse_tracking_remote_name(tracking_branch_name: Option<&str>) -> Option<String> {
    let first = tracking_branch_name?.split('/').next()?.trim();
    (!first.is_empty()).then(|| first.to_string())
}

/// Builds `[prefix…, paths…]` without copying the prefix into a `Vec<String>`.
fn interleaved<'a>(prefix: &[&'a str], paths: &[&'a str]) -> Vec<&'a str> {
    let mut args = prefix.to_vec();
    args.extend_from_slice(paths);
    args
}

/// `toBranchMutationFailure` (`gitCliRepo.ts:314`).
pub fn branch_mutation_failure(
    action: &str,
    branch_name: Value,
    created: bool,
    summary: Value,
    issues: Vec<Value>,
) -> Value {
    json!({
        "ok": false,
        "action": action,
        "branchName": branch_name,
        "didChange": false,
        "created": created,
        "summary": summary,
        "issues": issues,
    })
}

/// `toBranchMutationSuccess` (`gitCliRepo.ts:332`).
pub fn branch_mutation_success(
    action: &str,
    branch_name: &str,
    did_change: bool,
    created: bool,
    summary: Value,
) -> Value {
    json!({
        "ok": true,
        "action": action,
        "branchName": branch_name,
        "didChange": did_change,
        "created": created,
        "summary": summary,
        "issues": [],
    })
}
