//! The Rust counterpart of `gitCheckpointRepo.ts` (493 lines): checkpoint creation/diff/restore/delete.
//!
//! Core constraints (same as the TS comments gitCheckpointRepo.ts:240-250): do not pollute the user's real index (temporary
//! `GIT_INDEX_FILE`), do not create a user-visible branch or an ordinary commit (hidden ref
//! `refs/zcode/checkpoints/{hash}/{id}`), and the scope always follows the current workspace (pathspec).
//! Repository resolution reuses `nc-code-services-git` (there is only one rev-parse implementation); execution goes through this crate's
//! `GitRunner` (`GitCommandEnv::checkpoint_index` carries GIT_INDEX_FILE plus the five author-identity fields,
//! matching `gitCheckpointHelpers.ts:184-192` field by field).

use std::collections::HashMap;
use std::path::Path;

use nc_code_service_core::{AppPaths, ServiceError};
use nc_code_services_git::cmd::GitCommandProvider;
use nc_code_services_git::path::repo_relative_to_absolute;
use nc_code_services_git::repo::{GitRepo, ResolvedRepository};
use serde_json::{json, Value};

use crate::gitcmd::{ensure_succeeded, GitCommandEnv, GitRunner};
use crate::helpers::{
    build_affected_repo_paths, checkpoint_ref_name, merge_checkpoint_diff, parse_ls_tree,
    parse_name_status, parse_numstat, paths_to_remove_after_restore, restore_repo_paths,
    workspace_pathspec,
};

pub struct CheckpointRepo {
    runner: GitRunner,
    git: std::sync::Arc<GitRepo>,
    paths: AppPaths,
}

impl CheckpointRepo {
    pub fn new(paths: AppPaths) -> Self {
        Self {
            runner: GitRunner::new(),
            // GitCommandProvider::new() already returns Arc<..>.
            git: GitRepo::new(GitCommandProvider::new()),
            paths,
        }
    }

    /// `ensureRepository` (gitCheckpointRepo.ts:56-74): a single up-front check shared by all checkpoint operations,
    /// so the methods do not each decide for themselves and end up with inconsistent error semantics.
    async fn ensure_repository(
        &self,
        workspace_path: &str,
    ) -> Result<ResolvedRepository, ServiceError> {
        let resolution = self.git.resolve_repository(workspace_path).await;
        if !resolution.is_git_available {
            return Err(ServiceError::new(
                "Git binary is not available in the current environment.",
            ));
        }
        if !resolution.is_repository {
            return Err(ServiceError::new(
                "Workspace is not inside a Git repository.",
            ));
        }
        Ok(resolution)
    }

    /// `createCheckpoint` (gitCheckpointRepo.ts:257-331): temporary index (prewarm = copy the user
    /// index's stat cache → degrade to read-tree HEAD → fall back to an empty index) → `git add -A` →
    /// `write-tree` → `commit-tree` → a hidden ref pins the objects (prevents GC, and never writes the real index).
    pub async fn create_checkpoint(
        &self,
        workspace_path: &str,
        checkpoint_id: &str,
    ) -> Result<Value, ServiceError> {
        let resolution = self.ensure_repository(workspace_path).await?;
        let hash = self.paths.workspace_hash(workspace_path, None);
        let ref_name = checkpoint_ref_name(&hash, checkpoint_id);
        let index_root = self.paths.git_checkpoint_index_root_dir();
        let temp_index_dir =
            index_root.join(format!("index-{}-{}", std::process::id(), unique_tag()));
        tokio::fs::create_dir_all(&temp_index_dir)
            .await
            .map_err(|error| ServiceError::new(format!("checkpoint index dir: {error}")))?;
        let temp_index_path = temp_index_dir.join("index");
        let env = GitCommandEnv::checkpoint_index(&temp_index_path);
        let pathspec = workspace_pathspec(&resolution.workspace_in_repo_path);

        let outcome = self
            .build_checkpoint(&resolution, checkpoint_id, &temp_index_path, &env, pathspec)
            .await;

        // The temporary index only serves this build: a cleanup failure must not change the result of an already successful checkpoint.
        let _ = tokio::fs::remove_dir_all(&temp_index_dir).await;
        let commit_oid = outcome?;

        Ok(json!({
            "checkpointId": checkpoint_id,
            "workspacePath": workspace_path,
            "repoRoot": resolution.repo_root,
            "workspaceInRepoPath": resolution.workspace_in_repo_path,
            "createdAt": now_ms(),
            "refName": ref_name,
            "commitOid": commit_oid,
            "scope": "workspace",
        }))
    }

    /// The real git sequence (compared against TS entry by entry): prewarm → add → write-tree → commit-tree → update-ref.
    async fn build_checkpoint(
        &self,
        resolution: &ResolvedRepository,
        checkpoint_id: &str,
        temp_index_path: &Path,
        env: &GitCommandEnv,
        pathspec: &str,
    ) -> Result<String, ServiceError> {
        let repo_root = resolution.root();

        // Prewarm the temporary index: copy the user's real index so unchanged files take the stat-match fast path (the TS comments
        // state the performance constraint: with an empty index, add on a large repo hits the 15s timeout). If it cannot be read, use read-tree HEAD;
        // for a brand-new repository a read-tree failure is allowed (falling back to an empty index).
        let mut primed = false;
        let index_path = self
            .runner
            .run(
                repo_root,
                &["rev-parse", "--git-path", "index"],
                &GitCommandEnv::default(),
            )
            .await?;
        if index_path.exit_code == Some(0) {
            let index_rel = index_path.trimmed_stdout();
            if !index_rel.is_empty() {
                let user_index = repo_root.join(index_rel);
                if tokio::fs::copy(&user_index, temp_index_path).await.is_ok() {
                    primed = true;
                }
            }
        }
        if !primed {
            let read_tree = self
                .runner
                .run(repo_root, &["read-tree", "HEAD"], env)
                .await?;
            if read_tree.exit_code == Some(0) {
                primed = true;
            }
        }
        let _ = primed;

        let add = self
            .runner
            .run(repo_root, &["add", "-A", "--", pathspec], env)
            .await?;
        ensure_succeeded("git add checkpoint", &add, &[0])?;

        let tree = self.runner.run(repo_root, &["write-tree"], env).await?;
        ensure_succeeded("git write-tree checkpoint", &tree, &[0])?;

        let commit = self
            .runner
            .run(
                repo_root,
                &[
                    "commit-tree",
                    tree.trimmed_stdout(),
                    "-m",
                    &format!("zcode checkpoint {checkpoint_id}"),
                ],
                env,
            )
            .await?;
        ensure_succeeded("git commit-tree checkpoint", &commit, &[0])?;
        let commit_oid = commit.trimmed_stdout().to_string();

        // The hidden ref is the checkpoint's long-lived reference: it prevents GC and lets diff/restore/delete find the checkpoint back by ref.
        let hash = self.paths.workspace_hash(&resolution.workspace_path, None);
        let ref_name = checkpoint_ref_name(&hash, checkpoint_id);
        let update_ref = self
            .runner
            .run(
                repo_root,
                &["update-ref", &ref_name, &commit_oid],
                &GitCommandEnv::default(),
            )
            .await?;
        ensure_succeeded("git update-ref checkpoint", &update_ref, &[0])?;
        Ok(commit_oid)
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis() as i64)
        .unwrap_or_default()
}

fn unique_tag() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default()
}

/// For the `service` layer to do absolute ↔ repo-relative conversion (needed by conflict detection).
pub fn absolute_from_repo(repo_root: &str, repo_relative: &str) -> String {
    repo_relative_to_absolute(repo_root, repo_relative)
}

/// A type alias visible only when the tests verify the collection utilities (avoids leaking the private types behind parse_*).
pub type CheckpointStatMap = HashMap<String, (i64, i64)>;

impl CheckpointRepo {
    /// `computeCheckpointDiff` (gitCheckpointRepo.ts:81-130): answers only the pure file question ——
    /// from `from` to `to`, which files inside the workspace scope changed how (name-status + numstat).
    pub async fn diff_checkpoints(
        &self,
        from: &Value,
        to: &Value,
        workspace_path: &str,
    ) -> Result<Value, ServiceError> {
        let resolution = self.ensure_repository(workspace_path).await?;
        let pathspec = workspace_pathspec(&resolution.workspace_in_repo_path);
        let from_oid = oid_of(from)?;
        let to_oid = oid_of(to)?;
        let repo_root = resolution.root();

        let name_status = self
            .runner
            .run(
                repo_root,
                &[
                    "diff",
                    "--name-status",
                    "--find-renames",
                    "-z",
                    from_oid,
                    to_oid,
                    "--",
                    pathspec,
                ],
                &GitCommandEnv::default(),
            )
            .await?;
        ensure_succeeded("git diff --name-status checkpoint", &name_status, &[0])?;

        let numstat = self
            .runner
            .run(
                repo_root,
                &[
                    "diff",
                    "--numstat",
                    "--find-renames",
                    "-z",
                    from_oid,
                    to_oid,
                    "--",
                    pathspec,
                ],
                &GitCommandEnv::default(),
            )
            .await?;
        ensure_succeeded("git diff --numstat checkpoint", &numstat, &[0])?;

        Ok(merge_checkpoint_diff(
            &resolution.repo_root,
            &resolution.workspace_in_repo_path,
            &string_of(from, "checkpointId")?,
            &string_of(to, "checkpointId")?,
            parse_name_status(&name_status.stdout),
            &parse_numstat(&numstat.stdout),
        ))
    }

    /// `collectWorkspaceConflicts` (gitCheckpointRepo.ts:134-234): checks only whether the paths a restore would touch
    /// still equal fromCheckpoint, rather than blanket-declaring the whole repository unrestorable:
    /// - expected to exist but absent on disk → `missing-in-worktree`
    /// - expected to be absent but present on disk → `unexpected-file-in-worktree`
    /// - file/symlink kind mismatch → `type-mismatch`
    /// - blob hash differs (content changed) → `content-mismatch`
    /// - on a repeated path the later entry overrides the earlier one's value while keeping the first occurrence's position (JS Map semantics).
    pub async fn collect_conflicts(
        &self,
        workspace_path: &str,
        from: &Value,
        affected: &[String],
    ) -> Result<Vec<Value>, ServiceError> {
        if affected.is_empty() {
            return Ok(Vec::new());
        }
        let resolution = self.ensure_repository(workspace_path).await?;
        let repo_root = resolution.root();
        let from_oid = oid_of(from)?;

        let mut args: Vec<&str> = vec!["ls-tree", "-r", "-z", from_oid, "--"];
        args.extend(affected.iter().map(String::as_str));
        let tree = self
            .runner
            .run(repo_root, &args, &GitCommandEnv::default())
            .await?;
        ensure_succeeded("git ls-tree checkpoint paths", &tree, &[0])?;
        let entries = parse_ls_tree(&tree.stdout);

        let workspace_in_repo = resolution.workspace_in_repo_path.clone();
        let mut conflicts: Vec<Value> = Vec::new();
        let mut seen: HashMap<String, usize> = HashMap::new();
        let push =
            |conflicts: &mut Vec<Value>, seen: &mut HashMap<String, usize>, conflict: Value| {
                let key = conflict
                    .get("repoRelativePath")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                match seen.get(&key) {
                    Some(&position) => conflicts[position] = conflict,
                    None => {
                        seen.insert(key, conflicts.len());
                        conflicts.push(conflict);
                    }
                }
            };

        for path in affected {
            let absolute = repo_relative_to_absolute(&resolution.repo_root, path);
            let workspace_relative = to_workspace_relative(path, &workspace_in_repo);
            let Some(expected) = entries.get(path) else {
                if std::fs::symlink_metadata(&absolute).is_err() {
                    continue;
                }
                push(
                    &mut conflicts,
                    &mut seen,
                    conflict_json(
                        &absolute,
                        path,
                        &workspace_relative,
                        "unexpected-file-in-worktree",
                    ),
                );
                continue;
            };
            let Ok(metadata) = std::fs::symlink_metadata(&absolute) else {
                push(
                    &mut conflicts,
                    &mut seen,
                    conflict_json(&absolute, path, &workspace_relative, "missing-in-worktree"),
                );
                continue;
            };
            let expects_symlink = expected.mode == "120000";
            if metadata.is_dir() || (expects_symlink && !metadata.file_type().is_symlink()) {
                push(
                    &mut conflicts,
                    &mut seen,
                    conflict_json(&absolute, path, &workspace_relative, "type-mismatch"),
                );
                continue;
            }
            let hash = self
                .runner
                .run(
                    repo_root,
                    &["hash-object", "--no-filters", &absolute],
                    &GitCommandEnv::default(),
                )
                .await?;
            ensure_succeeded("git hash-object checkpoint verify", &hash, &[0])?;
            if hash.trimmed_stdout() == expected.object_id {
                continue;
            }
            // Do not compare weak signals such as timestamps/size: only a matching blob hash counts as "still on the fromCheckpoint baseline".
            push(
                &mut conflicts,
                &mut seen,
                conflict_json(&absolute, path, &workspace_relative, "content-mismatch"),
            );
        }
        Ok(conflicts)
    }

    /// `restoreBetweenCheckpoints` (gitCheckpointRepo.ts:344-461): a three-way comparison ——
    /// the caller declares disk=from, target=to, and reality may already have been changed: report conflicts first (only force overwrites),
    /// the restore touches only the worktree (never the real index), deletes and rename old paths are handled explicitly, and the result is verified at the end.
    pub async fn restore_between(
        &self,
        workspace_path: &str,
        from: &Value,
        to: &Value,
        force: bool,
    ) -> Result<Value, ServiceError> {
        let resolution = self.ensure_repository(workspace_path).await?;
        let diff = self.diff_checkpoints(from, to, workspace_path).await?;
        let files: Vec<Value> = diff
            .get("files")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let affected = build_affected_repo_paths(&resolution.repo_root, &files);
        if affected.is_empty() {
            return Ok(json!({"success": true, "restoredPaths": []}));
        }

        let conflicts = self
            .collect_conflicts(workspace_path, from, &affected)
            .await?;
        if !conflicts.is_empty() && !force {
            // Refuse to write by default: surface the conflicts to the user instead of silently overwriting their newer changes underneath.
            return Ok(json!({"success": false, "conflicts": conflicts}));
        }

        let restore_paths = restore_repo_paths(&files);
        if !restore_paths.is_empty() {
            let to_oid = oid_of(to)?;
            let source = format!("--source={to_oid}");
            let mut args: Vec<&str> = vec!["restore", &source, "--worktree", "--"];
            args.extend(restore_paths.iter().map(String::as_str));
            let restore = self
                .runner
                .run(resolution.root(), &args, &GitCommandEnv::default())
                .await?;
            ensure_succeeded("git restore checkpoint", &restore, &[0])?;
        }

        for path in paths_to_remove_after_restore(&files) {
            // TS rm(path, {force, recursive}): directories can be deleted too, and a missing path is not an error.
            let _ = std::fs::remove_file(&path);
            if std::path::Path::new(&path).is_dir() {
                let _ = std::fs::remove_dir_all(&path);
            }
        }

        let verify = self
            .collect_conflicts(workspace_path, to, &affected)
            .await?;
        if !verify.is_empty() {
            // Should never happen in theory: returning success anyway would cement the wrong state, so raise the error directly.
            return Err(ServiceError::new("Checkpoint restore verification failed."));
        }

        let restored: Vec<Value> = files
            .iter()
            .filter_map(|file| file.get("path").cloned())
            .collect();
        Ok(json!({"success": true, "restoredPaths": restored}))
    }

    /// `deleteCheckpoint` (gitCheckpointRepo.ts:471-481): deletes only the hidden ref
    /// (the manifest is deleted by the store); exit 1 = the ref never existed, matching TS's allowed [0,1].
    pub async fn delete_ref(
        &self,
        workspace_path: &str,
        ref_name: &str,
    ) -> Result<(), ServiceError> {
        let resolution = self.ensure_repository(workspace_path).await?;
        let output = self
            .runner
            .run(
                resolution.root(),
                &["update-ref", "-d", ref_name],
                &GitCommandEnv::default(),
            )
            .await?;
        ensure_succeeded("git update-ref -d checkpoint", &output, &[0, 1])
    }
}

fn oid_of(meta: &Value) -> Result<&str, ServiceError> {
    meta.get("commitOid")
        .and_then(Value::as_str)
        .ok_or_else(|| ServiceError::invalid_params("checkpoint manifest requires commitOid"))
}

fn string_of(meta: &Value, key: &str) -> Result<String, ServiceError> {
    meta.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ServiceError::invalid_params(format!("checkpoint manifest requires {key}")))
}

fn to_workspace_relative(repo_relative: &str, workspace_in_repo_path: &str) -> String {
    nc_code_services_git::path::to_workspace_relative_git_path(
        repo_relative,
        workspace_in_repo_path,
    )
}

fn conflict_json(
    absolute_path: &str,
    repo_relative_path: &str,
    workspace_relative_path: &str,
    reason: &str,
) -> Value {
    json!({
        "path": absolute_path,
        "repoRelativePath": repo_relative_path,
        "workspaceRelativePath": workspace_relative_path,
        "reason": reason,
    })
}
